using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.Logging;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>What a completed turn produced (the analog of the Rust <c>TurnResult</c>). <see cref="Directive"/>
/// is the opaque client-side directive a host tool wrote onto the turn's <see cref="TurnContext"/>
/// (<c>null</c> ⇒ none; drained onto <c>eventual_response.directive</c>).</summary>
public sealed record TurnResult(string Reply, string MessageId, IReadOnlyList<JsonObject> Citations, JsonNode? Directive = null, TurnUsage? Usage = null);

/// <summary>Per-turn token accounting + cost carried onto <c>eventual_response.usage</c>. Accumulated
/// across every model call in the turn. The analog of the Rust reference's <c>protocol::TurnUsage</c>.
/// <para>
/// <see cref="CostUsd"/> is the gateway's authoritative per-request cost, summed across the turn's
/// model calls. It reaches here only because the host injects core's <c>GatewayChatClient</c>: the
/// cost exists ONLY in a response header, and the MEAI OpenAI adapter this replaced dropped headers,
/// which is why this was hardcoded to 0 for so long. Read straight off the engine's own
/// CostTracker, so a turn the gateway did not price still falls back to whatever local pricing
/// the engine has — 0 means "nothing priced it", not "free".
/// </para></summary>
public sealed record TurnUsage(double CostUsd, long PromptTokens, long CompletionTokens);

/// <summary>
/// Drives one <c>send_message</c> turn: load prior history, retrieve grounding knowledge, run the
/// C# engine (<see cref="SmoothAgent"/>) streaming, emit <c>stream_token</c> events, persist the
/// reply, and return the citations. The C# analog of the Rust server's <c>run_streaming_turn</c>.
/// (ACL-filtered retrieval, the rerank stage, and tool/HITL stream_chunks arrive in later phases.)
/// </summary>
public sealed class TurnRunner
{
    private const int AutoContextLimit = 3;
    private const int RerankCandidatePool = 15; // fetched before the reranker trims to AutoContextLimit
    private const int MaxPriorMessages = 50;
    private const int CitationSnippetMaxChars = 280;

    /// <summary><c>max_tokens</c> for the fast-model preamble — one short sentence. Pearl th-9a5794.</summary>
    private const int PreambleMaxTokens = 64;

    /// <summary>
    /// System prompt for the fast-model preamble (see <c>SMOOTH_AGENT_PREAMBLE_MODEL</c>). One short
    /// present-tense sentence describing intent — no answer (it is generated WITHOUT the tool result),
    /// no greeting, no promises. Byte-identical to the Rust/Python/TS servers' prompt.
    /// </summary>
    private const string PreambleSystemPrompt =
        "You are the assistant's voice while it works. " +
        "In ONE short present-tense sentence (max ~12 words), tell the user what you're about to do to help with their message. " +
        "Do NOT answer the question, do NOT greet, do NOT promise a specific result or outcome. " +
        "Example: \"Let me pull up your recent conversations.\" " +
        "Reply with only that sentence — no quotes, no preamble, no markdown.";

    /// <summary>Env var enabling the preamble. Unset / empty / whitespace ⇒ the feature is OFF and no
    /// extra LLM call is ever made.</summary>
    private const string PreambleModelEnvVar = "SMOOTH_AGENT_PREAMBLE_MODEL";

    private readonly IChatClient _chatClient;
    private readonly ISessionStore _store;
    private readonly IKnowledgeBase? _knowledge;
    private readonly IReranker? _reranker;
    private readonly string _systemPrompt;
    private readonly IReadOnlyList<AITool> _tools;
    private readonly IReadOnlyList<IToolHook> _toolHooks;
    private readonly IReadOnlyList<string> _confirmTools;
    private readonly ConfirmationRegistry? _confirmations;
    private readonly InteractionCatalog? _interactions;
    private readonly InteractionParkRegistry? _interactionPark;
    private readonly SessionIdentityRegistry? _interactionEffects;
    private readonly IReadOnlyCollection<string> _capabilities;
    private readonly AgentConfig _agentConfig;
    private readonly IWorkflowJudge? _judge;
    private readonly TurnLimits _limits;
    private readonly ILogger? _logger;
    private readonly IChatClient _preambleChatClient;

    /// <summary>
    /// The fire-and-forget preamble task from the most recent <see cref="RunAsync(string,string,string,Action{JsonObject},string,CancellationToken,IReadOnlyList{UserImage},IReadOnlyList{UserFile},string)"/>
    /// (a completed task when the feature is off). Exposed purely so tests — and diagnostics — can
    /// observe when the parallel preamble has finished; the turn itself NEVER awaits it, so it can
    /// neither delay nor fail the answer.
    /// </summary>
    public Task PreambleCompleted { get; private set; } = Task.CompletedTask;

    /// <summary>How long a parked write-confirmation waits for a <c>confirm_tool_action</c> before the
    /// gate gives up and treats the tool as denied. Generous (5 min) because a human is in the loop —
    /// the same constant, for the same reason, as the Rust reference's <c>CONFIRMATION_TIMEOUT</c> and
    /// the interaction park's <see cref="RequestInteractionTool.ParkTimeout"/>.</summary>
    public static readonly TimeSpan DefaultConfirmationTimeout = TimeSpan.FromSeconds(300);

    /// <summary>The confirmation park's backstop (<see cref="DefaultConfirmationTimeout"/> unless a host
    /// — or a test — narrows it).</summary>
    public TimeSpan ConfirmationTimeout { get; init; } = DefaultConfirmationTimeout;

    public TurnRunner(IChatClient chatClient, ISessionStore store, IKnowledgeBase? knowledge = null, string? systemPrompt = null, IReranker? reranker = null, IReadOnlyList<AITool>? tools = null, IReadOnlyList<string>? confirmTools = null, ConfirmationRegistry? confirmations = null, AgentConfig? agentConfig = null, IWorkflowJudge? judge = null, TurnLimits? limits = null, ILogger? logger = null, IChatClient? preambleChatClient = null, IReadOnlyList<IToolHook>? toolHooks = null, InteractionCatalog? interactions = null, InteractionParkRegistry? interactionPark = null, IReadOnlyCollection<string>? capabilities = null, SessionIdentityRegistry? interactionEffects = null)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _knowledge = knowledge;
        _reranker = reranker;
        _systemPrompt = systemPrompt ??
            "You are a helpful customer support agent. Answer using only the knowledge provided to you; if it is not there, say you don't know.";
        _tools = tools ?? Array.Empty<AITool>();
        // Tool-call hooks (surveillance / redaction) applied to every turn's tool registry. Empty (the
        // default) ⇒ no hooks, behavior unchanged. Mirrors the Rust server installing NarcHook on the
        // operative's ToolRegistry.
        _toolHooks = toolHooks ?? Array.Empty<IToolHook>();
        // Tool-name substrings that require human approval before they run (empty → HITL off,
        // behavior unchanged). Matched by substring like the Rust/Python gate.
        _confirmTools = confirmTools ?? Array.Empty<string>();
        // The session-keyed pending-confirmation registry the gate parks on (null → HITL off).
        _confirmations = confirmations;
        // Rich Interactions: the hosted kinds catalog + the session-keyed park registry the raise tools
        // park on, and the session's declared render capabilities (from `supports`). Any null ⇒ no
        // interaction tools are registered → behavior identical to before Rich Interactions.
        _interactions = interactions;
        _interactionPark = interactionPark;
        // The session-contact overlay a kind's ApplyEffect stamps on a valid conversational-fallback
        // submit (null ⇒ no effect is run; the rich-frame path runs its own via the FrameDispatcher).
        _interactionEffects = interactionEffects;
        _capabilities = capabilities ?? Array.Empty<string>();
        // Per-agent config: instructions.prompt overrides the default persona; conversation_workflow
        // drives the guided-agency flow. Empty (the default) ⇒ the org/default persona, unchanged.
        _agentConfig = agentConfig ?? AgentConfig.Empty;
        // Post-turn workflow judge (null ⇒ no workflow advancement even if a workflow is configured;
        // the current step is still rendered, it just never advances). Wired by the host.
        _judge = judge;
        // Per-turn output-token budget + agentic-iteration cap, plus the resolved model's hard output
        // ceiling. Absent ⇒ the raised server defaults (max_tokens 8192, iterations 20; EPIC th-1cc9fa).
        _limits = limits ?? TurnLimits.Default;
        // Optional logger — used to surface a warning when knowledge retrieval degrades (null ⇒ silent).
        _logger = logger;
        // The client the parallel preamble calls. Defaults to the turn's own client — same gateway,
        // same key — with only the model id and max output tokens overridden per call (mirrors the
        // Rust runner cloning the turn's LlmConfig). A separate instance is only ever injected by tests.
        _preambleChatClient = preambleChatClient ?? _chatClient;
    }

    /// <summary>
    /// The configured chat model, emitted as <c>gen_ai.request.model</c> on the turn span. Read from
    /// the SAME env chain the host resolves the gateway model from (<c>SMOOTH_AGENT_MODEL</c> →
    /// <c>SMOOAI_MODEL</c> → <c>SMOOTH_MODEL</c> → default), mirroring how <see cref="PreambleModel"/>
    /// reads its own env — the model is this server's own config surface, so the env IS the source of
    /// truth (the injected <see cref="IChatClient"/> exposes no model metadata to read off).
    /// </summary>
    // ponytail: env is the model config surface (host reads the same chain); no plumbing a model
    // string through FrameDispatcher just for a span tag. Thread it explicitly if a host ever injects
    // a client whose model diverges from the env.
    private static string ConfiguredModel() =>
        (Environment.GetEnvironmentVariable("SMOOTH_AGENT_MODEL")
         ?? Environment.GetEnvironmentVariable("SMOOAI_MODEL")
         ?? Environment.GetEnvironmentVariable("SMOOTH_MODEL"))?.Trim() is { Length: > 0 } model
            ? model
            : "claude-haiku-4-5";

    /// <summary>Emit a <c>gen_ai.tool</c> child span (parented to the ambient turn span) for one tool
    /// call, carrying the tool name and its redacted JSON arguments. No-op when nothing is sampling
    /// <see cref="Telemetry.Source"/> (<c>StartActivity</c> returns null).</summary>
    private static void EmitToolSpan(FunctionCallContent call, string conversationId)
    {
        using var toolSpan = Telemetry.Source.StartActivity(Telemetry.SpanTool);
        if (toolSpan is null)
        {
            return;
        }
        // The OTLP ingest builds a span's attributes from the resource attrs plus THAT span's own,
        // with no inheritance from the parent — so a child repeats its identifiers or it cannot be
        // joined. Omitting gen_ai.system is worse than losing the join: the ingest's LLM-event gate
        // keys on it, so bare tool spans are DISCARDED. Rust's were, for their entire existence.
        toolSpan.SetTag(Telemetry.GenAiSystem, Telemetry.SystemName);
        toolSpan.SetTag(Telemetry.GenAiOperationName, Telemetry.OperationTool);
        toolSpan.SetTag(Telemetry.GenAiConversationId, conversationId);
        toolSpan.SetTag(Telemetry.GenAiToolName, call.Name);
        var args = call.Arguments is null ? "{}" : JsonSerializer.Serialize(call.Arguments);
        toolSpan.SetTag(Telemetry.GenAiToolArguments, Telemetry.RedactToolArguments(args));
    }

    /// <summary>The configured preamble model id, or <c>null</c> when the feature is off (env unset,
    /// empty, or whitespace). Read per turn so a host can flip it without a restart.</summary>
    private static string? PreambleModel() =>
        Environment.GetEnvironmentVariable(PreambleModelEnvVar)?.Trim() is { Length: > 0 } model ? model : null;

    /// <summary>
    /// The system prompt for this turn: per-agent <c>instructions.prompt</c> when present (else the
    /// org/default persona), with the personality line and the current workflow step's
    /// <c>&lt;ConversationWorkflow&gt;</c> section appended. Mirrors the monorepo's prompt assembly
    /// (agentInstructions + workflowSection). With an empty <see cref="AgentConfig"/> this returns the
    /// default persona verbatim — behavior unchanged.
    /// </summary>
    private string BuildSystemPrompt(string? currentStepId, bool isFirstTurn, string? skillSection = null)
    {
        var basePrompt = string.IsNullOrWhiteSpace(_agentConfig.InstructionsPrompt) ? _systemPrompt : _agentConfig.InstructionsPrompt!;
        var builder = new StringBuilder(basePrompt);
        if (!string.IsNullOrWhiteSpace(_agentConfig.Personality))
        {
            builder.Append("\n\nPERSONALITY: ").Append(_agentConfig.Personality);
        }
        // First-turn greeting seed (mirrors the Python/TS lanes): weave the greeting into the opening
        // reply, not a separate message — this server has no message-seed path. Only on the first turn,
        // so the agent doesn't re-greet mid-conversation.
        if (isFirstTurn && !string.IsNullOrWhiteSpace(_agentConfig.Greeting))
        {
            builder.Append("\n\n<GreetingAwareness>\nThis is your first reply in this conversation. Open with a natural, brief variant of: \"")
                .Append(_agentConfig.Greeting)
                .Append("\" — then address the user's message in the same reply. Do NOT repeat the greeting verbatim, and do not reintroduce yourself later.\n</GreetingAwareness>");
        }
        if (_agentConfig.Workflow is not null)
        {
            var section = Workflows.RenderPromptSection(_agentConfig.Workflow, currentStepId);
            if (section.Length > 0)
            {
                builder.Append("\n\n").Append(section);
            }
        }
        // The turn's invoked skill (`send_message.skill`), appended LAST so it is the most salient
        // instruction the model carries into the turn. Null for an ordinary turn ⇒ byte-for-byte unchanged.
        if (!string.IsNullOrEmpty(skillSection))
        {
            builder.Append("\n\n").Append(skillSection);
        }
        return builder.ToString();
    }

    /// <summary>True when <paramref name="toolName"/> matches a confirmation-gated pattern (substring,
    /// like the Rust/Python gate). Only meaningful when a confirmation registry is wired.</summary>
    private bool IsGated(string toolName) =>
        _confirmations is not null && _confirmTools.Any(pattern => toolName.Contains(pattern, StringComparison.Ordinal));

    /// <summary>True when <paramref name="toolName"/> is one of this turn's <c>request_&lt;kind&gt;</c>
    /// raise tools. Their toolCall chunk is deferred out of the stream loop and re-emitted by the tool
    /// itself — after <c>interaction_required</c> on the park path.</summary>
    private bool IsInteractionRaise(string toolName) =>
        _interactions is not null && _interactions.Kinds.Any(kind => kind.ToolName == toolName);

    public Task<TurnResult> RunAsync(string conversationId, string requestId, string userMessage, Action<JsonObject> sink, CancellationToken cancellationToken = default) =>
        RunAsync(conversationId, requestId, userMessage, sink, sessionId: conversationId, cancellationToken);

    /// <summary>
    /// Run one turn. <c>images</c> (from <c>send_message.images[]</c>) attach to the user turn as OpenAI
    /// <c>image_url</c> content parts; <c>files</c> (from <c>send_message.files[]</c>) surface on the
    /// per-turn <see cref="TurnContext"/> for host tools and are never sent to the model. Both empty/null
    /// ⇒ a text-only turn, unchanged.
    /// </summary>
    public async Task<TurnResult> RunAsync(string conversationId, string requestId, string userMessage, Action<JsonObject> sink, string sessionId, CancellationToken cancellationToken = default, IReadOnlyList<UserImage>? images = null, IReadOnlyList<UserFile>? files = null, string? skillSection = null)
    {
        // OpenTelemetry GenAI turn span (`gen_ai.chat`), mirroring the Rust runner's turn_span: wraps the
        // whole turn so the tool child spans nest under it; token usage is recorded onto it once the
        // stream ends. Null (no-op) unless something is sampling Telemetry.Source — the OTel SDK in the
        // host (env-gated on OTEL_EXPORTER_OTLP_ENDPOINT) or a test's ActivityListener.
        using var turnActivity = Telemetry.Source.StartActivity(Telemetry.SpanChat);
        turnActivity?.SetTag(Telemetry.GenAiSystem, Telemetry.SystemName);
        turnActivity?.SetTag(Telemetry.GenAiOperationName, Telemetry.OperationChat);
        turnActivity?.SetTag(Telemetry.GenAiRequestModel, ConfiguredModel());
        turnActivity?.SetTag(Telemetry.GenAiConversationId, conversationId);
        turnActivity?.SetTag(Telemetry.GenAiAgentName, Telemetry.AgentName);

        // 1. Auto-context citations (what grounded the answer). Mirrors the Rust auto_sources.
        //    With a reranker configured, fetch a wider candidate pool and let it reorder down to
        //    the top few before they become citations; without one, fetch exactly the top few
        //    (behavior unchanged — the rerank stage is opt-in).
        var citations = new List<JsonObject>();
        // The knowledge base handed to the engine for its own RAG grounding. Nulled if retrieval fails
        // this turn so the engine's internal query doesn't re-hit the same dead dependency and throw.
        var knowledgeForTurn = _knowledge;
        if (_knowledge is not null)
        {
            var fetchLimit = _reranker is not null ? RerankCandidatePool : AutoContextLimit;
            IReadOnlyList<KnowledgeResult>? candidates = null;
            try
            {
                candidates = await _knowledge.QueryAsync(userMessage, fetchLimit, cancellationToken).ConfigureAwait(false);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                // Retrieval (the embedding gateway / vector store) is best-effort grounding: when it is
                // down the turn must DEGRADE to ungrounded, not die with INTERNAL_ERROR. Drop grounding
                // for this turn — no citations — and don't hand the failing store to the engine (its own
                // RAG query would re-hit the dead dependency and throw). The user still gets an answer.
                // Cancellation still propagates (excluded above).
                _logger?.LogWarning(ex, "Knowledge retrieval failed for conversation {ConversationId}; proceeding with empty grounding.", conversationId);
                knowledgeForTurn = null;
            }
            if (candidates is not null)
            {
                IReadOnlyList<KnowledgeResult> hits;
                try
                {
                    hits = await Rerankers.ApplyOptionalAsync(_reranker, userMessage, candidates, AutoContextLimit, cancellationToken).ConfigureAwait(false);
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    // The reranker is an opt-in retrieval-QUALITY stage (the GatewayReranker hits the
                    // network) — a transient failure there must not deny the user an answer. Fall back
                    // to the upstream retrieval order, truncated. Cancellation still propagates.
                    hits = candidates.Take(AutoContextLimit).ToArray();
                }
                foreach (var hit in hits)
                {
                    var url = hit.Source.StartsWith("http://", StringComparison.Ordinal) || hit.Source.StartsWith("https://", StringComparison.Ordinal) ? hit.Source : null;
                    citations.Add(ProtocolEvents.Citation(hit.DocumentId, hit.Source, url, Truncate(hit.Chunk, CitationSnippetMaxChars), hit.Score));
                }
            }
        }

        // 2. Build the agent + replay prior history as memory (before persisting this turn's inbound).
        //    Registered tools (default none) are passed straight to the engine's agentic loop; the
        //    streaming block below already translates the resulting tool-call/result events into
        //    stream_chunks, so enabling tools is purely a matter of supplying them here.
        //
        //    The system prompt is assembled per-agent: instructions.prompt (when configured) plus the
        //    current workflow step's <ConversationWorkflow> section. The step pointer is persisted per
        //    conversation, so a multi-turn workflow resumes where it left off.
        var currentStepId = _agentConfig.Workflow is null
            ? null
            : await _store.GetWorkflowStepAsync(conversationId, cancellationToken).ConfigureAwait(false);
        // Prior history drives both the memory replay (below) and the first-turn greeting seed: an
        // empty history means this is the agent's first reply, so the greeting section is rendered.
        var priorMessages = await _store.ListMessagesAsync(conversationId, MaxPriorMessages, cancellationToken).ConfigureAwait(false);
        var resolvedPrompt = BuildSystemPrompt(currentStepId, isFirstTurn: priorMessages.Count == 0, skillSection: skillSection);
        // MaxOutputTokens is clamped to the model's ModelMaxOutputTokens ceiling by the engine so a
        // budget never exceeds what the model can physically emit (EPIC th-1cc9fa). The raised defaults
        // (8192 / 20) give reasoning models room to think AND answer, and iterations to actually use tools.
        var options = new AgentOptions
        {
            Instructions = resolvedPrompt,
            Knowledge = knowledgeForTurn,
            MaxIterations = _limits.MaxIterations,
            MaxOutputTokens = _limits.MaxTokens,
            ModelMaxOutputTokens = _limits.ModelMaxOutputTokens,
        };
        foreach (var tool in _tools)
        {
            options.Tools.Add(tool);
        }
        foreach (var hook in _toolHooks)
        {
            options.ToolHooks.Add(hook);
        }

        // Write-confirmation HITL: when configured with tool patterns AND a registry is present,
        // install an IHumanGate that PARKS the turn before a gated tool runs — emit
        // write_confirmation_required, then await the client's verdict via the session-keyed
        // registry. With no patterns (the default) no gate is installed → no tool ever parks →
        // behavior identical to before HITL. The gate keys its pending task by sessionId, so a
        // confirm_tool_action frame (also keyed by sessionId) routes back here.
        if (_confirmTools.Count > 0 && _confirmations is not null)
        {
            var registry = _confirmations;
            var session = sessionId;
            var confirmTimeout = ConfirmationTimeout;
            options.RequiresApproval = call => _confirmTools.Any(p => call.Name.Contains(p, StringComparison.Ordinal));
            options.HumanGate = new DelegateHumanGate(async (HumanApprovalRequest req, CancellationToken ct) =>
            {
                // Park: register a fresh task, emit the confirmation event, then await the client's
                // confirm_tool_action. toolId is the tool name (one tool parks at a time — a stable
                // correlation key).
                //
                // Event ORDER matters for cross-language parity: the canonical (Rust) server emits
                // write_confirmation_required BEFORE the gated tool's stream_chunk(toolCall). The
                // engine, however, yields the FunctionCallContent before consulting the gate — so the
                // stream loop DEFERS a gated tool's stream_chunk (see IsGated) and we emit it HERE,
                // right after the confirmation prompt, to match.
                var pending = registry.Register(session);
                sink(ProtocolEvents.WriteConfirmationRequired(requestId, req.ToolName, req.Prompt));
                sink(ProtocolEvents.StreamChunk(requestId, req.ToolName, ToolCallStateFrom(req.ToolName, req.Arguments)));
                var approved = await AwaitConfirmation(pending, registry, session, confirmTimeout, ct).ConfigureAwait(false);
                return approved ? HumanApprovalResponse.Approve() : HumanApprovalResponse.Deny("user rejected the action");
            });
        }

        // Rich Interactions: register a per-kind raise tool (request_<kind>) for each hosted kind. A kind
        // whose render capability the session declared in `supports` gets the RICH path (park + emit
        // interaction_required); a kind without it gets the conversational FALLBACK (a directive the
        // model asks + submits via the submit_interaction tool). When any kind is in fallback, the
        // generic submit_interaction tool is registered too. Mirrors the Rust runner's interaction wiring.
        if (_interactions is not null && _interactionPark is not null && _interactions.Kinds.Count > 0)
        {
            var raised = new ConcurrentDictionary<string, JsonNode>();
            var anyFallback = false;
            foreach (var kind in _interactions.Kinds)
            {
                var rich = _capabilities.Contains(kind.Capability);
                anyFallback |= !rich;
                options.Tools.Add(new RequestInteractionTool(kind, rich, sink, requestId, sessionId, _interactionPark, raised));
            }
            if (anyFallback)
            {
                // Session-bound effect context so a conversational-fallback submit runs the kind's host
                // effect (identity_intake → stamp the session contact) exactly like the rich-frame path.
                var effect = _interactionEffects?.EffectContext(sessionId);
                options.Tools.Add(new SubmitInteractionTool(_interactions, raised, effect));
            }
        }

        var agent = new SmoothAgent(new CancelAwareChatClient(_chatClient), options);
        var thread = agent.GetNewThread();
        foreach (var message in priorMessages)
        {
            var role = message.Direction == MessageDirection.Outbound ? ChatRole.Assistant : ChatRole.User;
            thread.Add(new ChatMessage(role, message.Text));
        }

        // Multimodal: the engine builds the LIVE user turn from the `userMessage` STRING (it can carry no
        // content parts), so we can't attach images to it directly. Instead we seat an image-only user
        // message on the thread immediately before the text turn — the model sees the image(s) adjacent to
        // the question, no empty/duplicate message. Ephemeral (thread is rebuilt from the store each turn),
        // so images are per-turn and never persisted — the persisted user message stays the typed text.
        // Fail-soft: entries that don't map to an image part are dropped, never rejecting the turn.
        if (images is { Count: > 0 })
        {
            var parts = new List<AIContent>(images.Count);
            foreach (var image in images)
            {
                if (TryImageContent(image) is { } content)
                {
                    parts.Add(content);
                }
            }
            if (parts.Count > 0)
            {
                thread.Add(new ChatMessage(ChatRole.User, parts));
            }
        }

        // Per-turn tool context: the parsed files (host tools may land them in a workspace) plus the
        // directive sink a host tool writes onto. Published as the ambient TurnContext around the run so a
        // tool the engine invokes reaches it, then drained after the turn onto eventual_response.directive.
        var turnContext = new TurnContext(files);

        // 3. Persist the inbound user message.
        await _store.AppendMessageAsync(conversationId, MessageDirection.Inbound, userMessage, cancellationToken).ConfigureAwait(false);

        // 4. Stream the turn: a stream_token per text delta, and a stream_chunk per tool call /
        //    tool result (mirrors the Rust runner translating ToolCallStart/Complete events). Tool
        //    calls are deduped by callId (streaming can fragment them); results are labeled by
        //    looking the tool name back up from the call.
        var reply = new StringBuilder();
        long promptTokens = 0;
        long completionTokens = 0;
        var sawUsage = false;
        var toolNames = new Dictionary<string, string>();
        var emittedCalls = new HashSet<string>();

        // First-answer-token guard for the optional preamble: the stream loop below flips it on the
        // first real text delta so a slow preamble can never pop in AFTER the answer has begun. The
        // two run on different tasks, so it is an explicit shared box written with Interlocked and read
        // with Volatile — never a plain (or closure-captured) local.
        var answerStarted = new StrongBox<int>(0);

        // Fire the fast-model preamble in PARALLEL with the agent loop (no-op unless
        // SMOOTH_AGENT_PREAMBLE_MODEL is set). Best-effort and fully detached: it is never awaited, so
        // it cannot delay or gate the turn, and any failure is swallowed after a debug log — no error
        // event ever reaches the client. Its text is written straight to the sink and NEVER appended to
        // `reply`, so it is not persisted and never appears in eventual_response. Pearl th-9a5794.
        PreambleCompleted = Task.CompletedTask;
        if (PreambleModel() is { } preambleModel)
        {
            PreambleCompleted = Task.Run(
                async () =>
                {
                    try
                    {
                        ChatMessage[] prompt =
                        [
                            new(ChatRole.System, PreambleSystemPrompt),
                            new(ChatRole.User, userMessage), // the user's message only — no tool results
                        ];
                        var response = await _preambleChatClient.GetResponseAsync(
                            prompt,
                            new ChatOptions { ModelId = preambleModel, MaxOutputTokens = PreambleMaxTokens },
                            cancellationToken).ConfigureAwait(false);
                        var preambleText = response.Text?.Trim();
                        if (!string.IsNullOrEmpty(preambleText) && Volatile.Read(ref answerStarted.Value) == 0)
                        {
                            sink(ProtocolEvents.StreamPreamble(requestId, preambleText));
                        }
                    }
                    catch (Exception ex)
                    {
                        _logger?.LogDebug(ex, "Preamble generation failed (ignored).");
                    }
                },
                cancellationToken);
        }

        try
        {
            // Publish the per-turn context so any host tool the engine invokes below sees this turn's
            // files and can write a directive onto the sink. Restored when the run's enumeration ends.
            using var _ = turnContext.Enter();
            await foreach (var update in agent.RunStreamingAsync(userMessage, thread, cancellationToken).ConfigureAwait(false))
            {
                var text = update.Text;
                if (!string.IsNullOrEmpty(text))
                {
                    // The real answer has started — from here on the preamble task must stay silent.
                    // Flipped BEFORE the token is emitted so the guard can never lose the race.
                    Interlocked.Exchange(ref answerStarted.Value, 1);
                    reply.Append(text);
                    sink(ProtocolEvents.StreamToken(requestId, text));
                }

                foreach (var content in update.Contents)
                {
                    switch (content)
                    {
                        case FunctionCallContent call when emittedCalls.Add(call.CallId):
                            toolNames[call.CallId] = call.Name;
                            // `gen_ai.tool` child span (nests under the turn span), mirroring the Rust
                            // runner emitting one gen_ai.tool span per tool call with redacted args.
                            EmitToolSpan(call, conversationId);
                            // DEFER a parking tool's toolCall chunk: it is emitted from the park path
                            // AFTER write_confirmation_required / interaction_required, so the wire
                            // order matches the canonical (Rust) server. Ungated tools emit inline.
                            if (IsGated(call.Name) || IsInteractionRaise(call.Name))
                            {
                                break;
                            }
                            sink(ProtocolEvents.StreamChunk(requestId, call.Name, ToolCallState(call)));
                            break;
                        case FunctionResultContent result:
                            var name = toolNames.TryGetValue(result.CallId, out var resolved) ? resolved : "tool";
                            sink(ProtocolEvents.StreamChunk(requestId, name, ToolResultState(name, result)));
                            break;
                        // The engine's streaming path doesn't surface a terminal usage total the way
                        // the other ports' `done` event does, so accumulate the model's own usage
                        // chunks here — that IS the turn total once the stream ends.
                        case UsageContent usage:
                            promptTokens += usage.Details.InputTokenCount ?? 0;
                            completionTokens += usage.Details.OutputTokenCount ?? 0;
                            sawUsage = true;
                            break;
                    }
                }
            }
        }
        finally
        {
            // Turn over: drop any lingering pending confirmation so a stale entry can't mis-route a
            // later confirm_tool_action (mirrors the Rust clear at turn end). No-op when HITL is off.
            _confirmations?.Clear(sessionId);
            // Same for a lingering interaction park, so a stale entry can't mis-route a later
            // submit_interaction. No-op when Rich Interactions are off.
            _interactionPark?.Clear(sessionId);
        }

        // Token counts and cost on the turn span, recording only what was actually measured.
        //
        // `sawUsage` alone was NOT enough: a usage chunk carrying null counts sets it while both
        // totals resolve to 0 via `?? 0`, so this published `input_tokens = 0` on a grounded turn.
        // Every other engine guards on the counts themselves. Absent is honest; 0 is a lie.
        if (turnActivity is not null)
        {
            if (sawUsage && (promptTokens > 0 || completionTokens > 0))
            {
                turnActivity.SetTag(Telemetry.GenAiUsageInputTokens, promptTokens);
                turnActivity.SetTag(Telemetry.GenAiUsageOutputTokens, completionTokens);
            }

            // Cost is judged separately from the counts: the gateway reports it on an HTTP header
            // while usage arrives on an SSE chunk, so either can turn up without the other. A
            // non-positive cost becomes an explicit "unpriced" marker, never a confident $0.00.
            var costUsd = TurnUsageFrom(agent, sawUsage, promptTokens, completionTokens)?.CostUsd ?? 0;
            if (costUsd > 0 && double.IsFinite(costUsd))
            {
                turnActivity.SetTag(Telemetry.GenAiUsageCostUsd, costUsd);
            }
            else
            {
                turnActivity.SetTag(Telemetry.CostUnavailable, Telemetry.CostUnavailableUnpriced);
            }
        }

        // 5. Persist the outbound reply.
        var replyText = reply.ToString();
        var outbound = await _store.AppendMessageAsync(conversationId, MessageDirection.Outbound, replyText, cancellationToken).ConfigureAwait(false);

        // 6. Advance the conversation workflow. A cheap judge decides whether the current step's
        //    criteria were met this turn; on "yes" the pointer moves to the next step (explicit `next`
        //    or the following step in order), otherwise it stays put. Failure-tolerant: a judge that
        //    errors / returns skipped never moves the pointer, so a bad judge call can't strand the
        //    flow. No-op unless the agent has a workflow AND a judge is wired.
        if (_agentConfig.Workflow is not null && _judge is not null)
        {
            await AdvanceWorkflowAsync(conversationId, currentStepId, userMessage, replyText, cancellationToken).ConfigureAwait(false);
        }

        // Drain the directive sink (mirrors the Rust runner draining directive_sink after the turn). A host
        // tool that ran this turn may have written a client-side directive; null ⇒ none, so eventual_response
        // omits directive (back-compat). Last-write-wins is inherent — the sink holds the final write.
        return new TurnResult(
            replyText,
            outbound.Id,
            citations,
            turnContext.Directive,
            TurnUsageFrom(agent, sawUsage, promptTokens, completionTokens));
    }

    /// <summary>
    /// The turn's usage, preferring the ENGINE's own accounting. <c>SmoothAgent</c> folds the
    /// gateway cost itself on the streaming path (core#136) and exposes the totals on
    /// <see cref="SmoothAgent.LastRunResponse"/> once the stream is fully enumerated — which it
    /// is by the time we get here. Reading it beats re-summing the updates: same number, and it
    /// picks up the engine's local-pricing fallback for a turn the gateway did not price.
    /// <para>Falls back to the locally counted tokens when the engine reports nothing (an older
    /// core, or a stream that produced no terminal totals).</para>
    /// </summary>
    private static TurnUsage? TurnUsageFrom(SmoothAgent agent, bool sawUsage, long promptTokens, long completionTokens)
    {
        if (agent.LastRunResponse is { Cost: { } cost })
        {
            return new TurnUsage((double)cost.TotalCostUsd, cost.TotalPromptTokens, cost.TotalCompletionTokens);
        }
        return sawUsage ? new TurnUsage(0, promptTokens, completionTokens) : null;
    }

    /// <summary>
    /// Map a parsed <see cref="UserImage"/> to an OpenAI <c>image_url</c> content part — a
    /// <see cref="DataContent"/> for a <c>data:</c> URL or a <see cref="UriContent"/> for an
    /// <c>http(s)</c> one; any other scheme, or a build failure, yields <c>null</c> (fail-soft, the entry
    /// is dropped). The optional vision <c>detail</c> hint rides on <see cref="AIContent.AdditionalProperties"/>.
    /// </summary>
    private static AIContent? TryImageContent(UserImage image)
    {
        try
        {
            var url = image.Url?.Trim();
            if (string.IsNullOrEmpty(url))
            {
                return null;
            }

            AIContent content;
            if (url.StartsWith("data:", StringComparison.OrdinalIgnoreCase))
            {
                content = new DataContent(url, MediaTypeFromDataUri(url) ?? "image/*");
            }
            else if (url.StartsWith("http://", StringComparison.OrdinalIgnoreCase) || url.StartsWith("https://", StringComparison.OrdinalIgnoreCase))
            {
                content = new UriContent(url, MediaTypeFromExtension(url));
            }
            else
            {
                return null; // unsupported scheme → drop
            }

            if (!string.IsNullOrWhiteSpace(image.Detail))
            {
                (content.AdditionalProperties ??= new()).Add("detail", image.Detail);
            }
            return content;
        }
        catch
        {
            return null; // malformed URL / bad media type → drop, never fail the turn
        }
    }

    /// <summary>The media type declared inside a <c>data:&lt;mime&gt;;...</c> URL, or <c>null</c> if absent.</summary>
    private static string? MediaTypeFromDataUri(string dataUri)
    {
        var semicolon = dataUri.IndexOf(';', StringComparison.Ordinal);
        var comma = dataUri.IndexOf(',', StringComparison.Ordinal);
        var end = semicolon >= 0 ? semicolon : comma;
        if (end <= 5)
        {
            return null;
        }
        var mediaType = dataUri.Substring(5, end - 5).Trim();
        return mediaType.Length > 0 && mediaType.Contains('/', StringComparison.Ordinal) ? mediaType : null;
    }

    /// <summary>A best-effort image media type guessed from a remote URL's extension (default <c>image/jpeg</c>,
    /// which OpenAI-compatible providers ignore in favor of the fetched bytes — the hint just satisfies the
    /// <see cref="UriContent"/> contract).</summary>
    private static string MediaTypeFromExtension(string url)
    {
        var path = url;
        var query = path.IndexOfAny(['?', '#']);
        if (query >= 0)
        {
            path = path[..query];
        }
        var dot = path.LastIndexOf('.');
        var ext = dot >= 0 ? path[(dot + 1)..].ToLowerInvariant() : string.Empty;
        return ext switch
        {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "bmp" => "image/bmp",
            _ => "image/jpeg",
        };
    }

    /// <summary>
    /// Run the post-turn judge and persist the (possibly advanced) workflow step. Only a
    /// <see cref="WorkflowVerdict.Yes"/> advances the pointer — every other verdict (including a
    /// failed/skipped judge) leaves the conversation on the current step so the flow never freezes or
    /// jumps. Mirrors the monorepo's workflow-judge node.
    /// </summary>
    private async Task AdvanceWorkflowAsync(string conversationId, string? currentStepId, string userMessage, string replyText, CancellationToken cancellationToken)
    {
        var workflow = _agentConfig.Workflow!;
        var current = Workflows.ResolveCurrentStep(workflow, currentStepId);
        if (current is null)
        {
            return;
        }

        var verdict = await _judge!.JudgeAsync(workflow, current, userMessage, replyText, cancellationToken).ConfigureAwait(false);
        var resolvedStepId = current.Id;
        if (verdict == WorkflowVerdict.Yes)
        {
            var advance = Workflows.NextStep(workflow, current);
            if (advance is not null)
            {
                resolvedStepId = advance.Id;
            }
        }

        // Persist even when the pointer didn't move: a fresh conversation had no stored step, so this
        // records the resolved starting step (mirrors the TS node writing currentStepId every turn).
        await _store.SetWorkflowStepAsync(conversationId, resolvedStepId, cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// Await a parked write-confirmation, backstopped by <see cref="ConfirmationTimeout"/> and the turn's
    /// cancellation. Without a backstop a client that never answers — a closed tab that left the socket
    /// open, so no teardown ever runs <see cref="ConfirmationRegistry.RejectAll"/> — pins the
    /// connection's ONLY turn slot forever: every later <c>send_message</c> gets
    /// <c>TURN_IN_PROGRESS</c> and drain hangs. The interaction park already had this backstop
    /// (<see cref="RequestInteractionTool.ParkTimeout"/>); confirmations did not. th-acf8ea.
    ///
    /// <para>Times out DENIED — fail closed, exactly like the disconnect path; a write is never
    /// auto-approved. A verdict that lands in the same instant as the deadline still wins.</para>
    /// </summary>
    private static async Task<bool> AwaitConfirmation(
        Task<bool> pending,
        ConfirmationRegistry registry,
        string sessionId,
        TimeSpan timeout,
        CancellationToken cancellationToken)
    {
        try
        {
            var completed = await Task.WhenAny(pending, Task.Delay(timeout, cancellationToken)).ConfigureAwait(false);
            if (completed == pending)
            {
                return await pending.ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException)
        {
            // Turn cancelled/torn down — the registration is (or will be) discarded elsewhere; deny.
        }

        // Take the registration out so a later confirm_tool_action can't resolve a park nobody awaits
        // (it gets the same clean NO_PENDING_CONFIRMATION a duplicate confirm gets). A no-op when the
        // client's verdict beat us here — in which case honor that verdict rather than the deadline.
        registry.Resolve(sessionId, approved: false);
        return pending.IsCompletedSuccessfully && pending.Result;
    }

    /// <summary>The stream_chunk toolCall state built from a gated tool's name + already-parsed
    /// arguments (the shape the engine's <see cref="HumanApprovalRequest"/> carries). Used to emit a
    /// gated tool's deferred toolCall chunk from the HumanGate.</summary>
    private static JsonObject ToolCallStateFrom(string name, IDictionary<string, object?>? arguments) => new()
    {
        ["rawResponse"] = new JsonObject
        {
            ["toolCall"] = new JsonObject
            {
                ["name"] = name,
                ["arguments"] = arguments is null ? new JsonObject() : JsonSerializer.SerializeToNode(arguments),
            },
        },
    };

    private static string Truncate(string value, int max) => value.Length <= max ? value : value[..max];

    private static JsonObject ToolCallState(FunctionCallContent call) => new()
    {
        ["rawResponse"] = new JsonObject
        {
            ["toolCall"] = new JsonObject
            {
                ["name"] = call.Name,
                ["arguments"] = call.Arguments is null ? new JsonObject() : JsonSerializer.SerializeToNode(call.Arguments),
            },
        },
    };

    private static JsonObject ToolResultState(string name, FunctionResultContent result)
    {
        var resultText = result.Result?.ToString() ?? string.Empty;
        // The engine folds tool failures into the result string (see InvokeToolAsync); detect that
        // convention so the chunk's isError flag matches the Rust ToolCallComplete signal.
        var isError = resultText.StartsWith("Error:", StringComparison.Ordinal) || resultText.StartsWith("Denied by human:", StringComparison.Ordinal);
        return new JsonObject
        {
            ["rawResponse"] = new JsonObject
            {
                ["toolResult"] = new JsonObject { ["name"] = name, ["isError"] = isError, ["result"] = resultText },
            },
        };
    }
}

/// <summary>
/// Wraps the turn's chat client so a CANCELLED turn can never issue another model call — the
/// difference between a stop button and a mute button.
///
/// <para>The engine's agent loop has no cancellation check of its own: it folds every tool failure
/// back to the model as a result and iterates, including the denial the write-confirmation gate
/// returns once the turn is cancelled (<c>TryCancelActiveTurn</c> resolves the park as denied so it
/// cannot hang). The runner has already walked away by then and its sink is gagged, so the loop's
/// remaining output is discarded — but the loop itself keeps running, calling the model again and
/// acting on whatever it answers, on a turn the user stopped.</para>
///
/// <para>The live gateway client would fail that call on its own cancelled token, so this is not a
/// standing spend leak — it is that the server was RELYING on the transport to stop a cancelled
/// turn. Cancellation is cooperative here, so the loop is stopped at the one place it
/// re-enters shared state: the model call. Throwing on a cancelled token unwinds
/// <c>RunStreamingAsync</c> and the turn ends — the .NET analog of dropping the Rust turn future,
/// which is preemptive and needs no such guard. The runner's caller already treats
/// <see cref="OperationCanceledException"/> as a clean cancellation.</para>
///
/// <para>Per-turn and decorative only: <see cref="Dispose"/> does NOT dispose the wrapped client,
/// which the server owns and reuses across turns.</para>
/// </summary>
internal sealed class CancelAwareChatClient : IChatClient
{
    private readonly IChatClient _inner;

    public CancelAwareChatClient(IChatClient inner) => _inner = inner;

    public Task<ChatResponse> GetResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return _inner.GetResponseAsync(messages, options, cancellationToken);
    }

    public IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return _inner.GetStreamingResponseAsync(messages, options, cancellationToken);
    }

    public object? GetService(Type serviceType, object? serviceKey = null) => _inner.GetService(serviceType, serviceKey);

    public void Dispose()
    {
    }
}
