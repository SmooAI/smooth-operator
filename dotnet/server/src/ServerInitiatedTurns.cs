using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>What a server-initiated turn produced: the fresh conversation/session it ran in, plus the
/// underlying <see cref="TurnResult"/> (reply, message id, citations).</summary>
public sealed record ServerInitiatedTurnResult(string ConversationId, string SessionId, TurnResult Turn);

/// <summary>
/// A host-callable seam to start an agent turn <em>server-side</em> — without a client
/// <c>send_message</c> frame. The use case: something on the host (e.g. <c>POST /webhooks/datadog</c>
/// saying "investigate this alert") wants to open a conversation and run a turn, and have it persist
/// exactly like a client-initiated turn so a client that later lists or resumes that conversation
/// sees it identically.
///
/// It reuses the SAME path as the client flow: mint a conversation via the <see cref="ISessionStore"/>
/// (the analog of <c>create_conversation_session</c>), then drive one turn through the same
/// <see cref="TurnRunner"/> the <see cref="FrameDispatcher"/> uses — so the inbound user message and
/// the streamed reply land in the store's message log the same way. Registered by
/// <c>AddSmoothOperatorServer</c>.
/// </summary>
public interface IServerInitiatedTurns
{
    /// <summary>
    /// Create a fresh conversation for <paramref name="agentId"/> and run one turn on
    /// <paramref name="message"/> (the initiating context — the alert text, the instruction, etc.),
    /// persisting the inbound message + the agent's reply into the session store. Returns the new
    /// conversation/session ids so the host can reference (or a client can later resume) it.
    /// </summary>
    /// <param name="agentId">The agent to run as. Empty ⇒ the store mints one (matches the client path).</param>
    /// <param name="message">The initiating message/context the turn responds to.</param>
    /// <param name="sink">Optional stream event sink (<c>stream_token</c> / <c>stream_chunk</c>). The
    /// message log is persisted regardless; a host that has nowhere to push live events omits it.</param>
    /// <param name="requestId">Correlation id stamped on emitted events; a fresh GUID when omitted.</param>
    /// <param name="access">Access scope for knowledge grounding; <see cref="AccessContext.Anonymous"/>
    /// (org-public) when omitted — a webhook-initiated turn carries no end-user identity.</param>
    /// <param name="cancellationToken">Cancels the turn.</param>
    Task<ServerInitiatedTurnResult> StartTurnAsync(
        string agentId,
        string message,
        Action<JsonObject>? sink = null,
        string? requestId = null,
        AccessContext? access = null,
        CancellationToken cancellationToken = default);
}

/// <summary>
/// The reference <see cref="IServerInitiatedTurns"/>. Takes the SAME shared collaborators the
/// <see cref="FrameDispatcher"/> hands its <see cref="TurnRunner"/> (chat client, store, ACL-scoped
/// knowledge, persona, reranker, tools, per-agent config, workflow judge, limits) so a server-initiated
/// turn is byte-for-byte the client turn — minus the per-connection interactive concerns that make no
/// sense off a socket: write-confirmation HITL and OTP identity gating (a host-triggered turn is
/// trusted; there is no client to prompt). Tools are passed straight through (the trusted host may
/// register whatever the turn needs).
/// </summary>
public sealed class ServerInitiatedTurns : IServerInitiatedTurns
{
    private readonly IChatClient _chatClient;
    private readonly ISessionStore _store;
    private readonly IAccessKnowledge? _knowledge;
    private readonly IReranker? _reranker;
    private readonly string? _systemPrompt;
    private readonly IReadOnlyList<AITool> _tools;
    private readonly IAgentConfigResolver? _agentConfigResolver;
    private readonly IWorkflowJudge? _judge;
    private readonly TurnLimits _limits;

    public ServerInitiatedTurns(
        IChatClient chatClient,
        ISessionStore store,
        IAccessKnowledge? knowledge = null,
        string? systemPrompt = null,
        IReranker? reranker = null,
        IReadOnlyList<AITool>? tools = null,
        IAgentConfigResolver? agentConfigResolver = null,
        IWorkflowJudge? judge = null,
        TurnLimits? limits = null)
    {
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _knowledge = knowledge;
        _reranker = reranker;
        _systemPrompt = systemPrompt;
        _tools = tools ?? Array.Empty<AITool>();
        _agentConfigResolver = agentConfigResolver;
        _judge = judge;
        _limits = limits ?? TurnLimits.Default;
    }

    public async Task<ServerInitiatedTurnResult> StartTurnAsync(
        string agentId,
        string message,
        Action<JsonObject>? sink = null,
        string? requestId = null,
        AccessContext? access = null,
        CancellationToken cancellationToken = default)
    {
        requestId ??= Guid.NewGuid().ToString();

        // 1. Mint a fresh conversation server-side — the same store call create_conversation_session
        //    makes for a client. No live socket, so there is no session resume to consider.
        var session = await _store.CreateSessionAsync(agentId, userName: null, userEmail: null, cancellationToken).ConfigureAwait(false);

        // 2. Resolve per-agent config (instructions/workflow) by the session's agent, exactly like the
        //    client path. Null (no resolver / unknown agent) ⇒ the default persona, unchanged.
        AgentConfig? agentConfig = null;
        if (_agentConfigResolver is not null)
        {
            agentConfig = await _agentConfigResolver.ResolveAsync(session.AgentId, cancellationToken).ConfigureAwait(false);
        }

        // 3. Scope knowledge to the access context (default org-public), then run the turn through the
        //    SAME TurnRunner the dispatcher uses. RunAsync persists the inbound user message and the
        //    outbound reply into the store, so a client that later list_conversations / resumes this id
        //    sees it identically. HITL + OTP are intentionally omitted (no client to prompt).
        var scopedKnowledge = _knowledge?.ForAccess(access ?? AccessContext.Anonymous);
        var runner = new TurnRunner(_chatClient, _store, scopedKnowledge, _systemPrompt, _reranker, _tools, confirmTools: null, confirmations: null, agentConfig, _judge, _limits);

        // A sink is optional: with nowhere to push live events, discard them — the message log is the
        // durable surface a client reads.
        var result = await runner.RunAsync(session.ConversationId, requestId, message, sink ?? (_ => { }), session.SessionId, cancellationToken).ConfigureAwait(false);

        return new ServerInitiatedTurnResult(session.ConversationId, session.SessionId, result);
    }
}
