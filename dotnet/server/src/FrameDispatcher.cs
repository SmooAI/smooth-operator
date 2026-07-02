using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Routes an incoming protocol frame (by its <c>action</c> discriminator) to the right handler and
/// emits the response event(s) to <paramref>sink</paramref>. The C# analog of the Rust server's
/// <c>handle_frame</c>. Transport-agnostic: a WebSocket host calls <see cref="DispatchAsync"/> per
/// inbound frame and writes the sink's events back over the socket.
///
/// One dispatcher is bound to one connection's <see cref="AccessContext"/> (resolved from the
/// <c>?token=</c> slot), and retrieval for each turn is scoped to it — so ACL is enforced on the
/// live chat path, not just at ingest.
/// </summary>
public sealed class FrameDispatcher
{
    private readonly ISessionStore _store;
    private readonly IChatClient _chatClient;
    private readonly IAccessKnowledge? _knowledge;
    private readonly IReranker? _reranker;
    private readonly AccessContext _access;
    private readonly string? _systemPrompt;
    private readonly IReadOnlyList<AITool> _tools;
    private readonly IReadOnlyList<string> _confirmTools;
    private readonly ConfirmationRegistry _confirmations;
    private readonly IAgentConfigResolver? _agentConfigResolver;
    private readonly IWorkflowJudge? _judge;
    private readonly ISessionAuthenticator _authenticator;

    // In-flight spawned send_message turns. A turn that calls a confirmation-gated tool parks
    // awaiting a later confirm_tool_action frame, so the turn runs as a background Task (not awaited
    // inline) to keep the read loop free; the connection awaits these on teardown (graceful drain).
    private readonly object _turnsLock = new();
    private readonly HashSet<Task> _turnTasks = new();

    public FrameDispatcher(
        ISessionStore store,
        IChatClient chatClient,
        IAccessKnowledge? knowledge = null,
        AccessContext? access = null,
        string? systemPrompt = null,
        IReranker? reranker = null,
        IReadOnlyList<AITool>? tools = null,
        IReadOnlyList<string>? confirmTools = null,
        ConfirmationRegistry? confirmations = null,
        IAgentConfigResolver? agentConfigResolver = null,
        IWorkflowJudge? judge = null,
        ISessionAuthenticator? authenticator = null)
    {
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _knowledge = knowledge;
        _access = access ?? AccessContext.Anonymous;
        _systemPrompt = systemPrompt;
        _reranker = reranker;
        _tools = tools ?? Array.Empty<AITool>();
        // Tool-name patterns gated behind write-confirmation HITL (empty → no gating, behavior
        // unchanged). When a turn calls a tool whose name contains one of these, the server parks the
        // turn and emits write_confirmation_required until the client replies with confirm_tool_action.
        _confirmTools = confirmTools ?? Array.Empty<string>();
        // Session-keyed pending-confirmation registry shared with each spawned turn so a
        // confirm_tool_action frame resolves the verdict a parked turn awaits. One per connection.
        _confirmations = confirmations ?? new ConfirmationRegistry();
        // Per-agent config resolution (null ⇒ no per-agent instructions/workflow are applied; every
        // agent uses the default persona, unchanged) and the post-turn workflow judge.
        _agentConfigResolver = agentConfigResolver;
        _judge = judge;
        // Identity-verification seam for end_user-level tools on public agents. Default fails closed.
        _authenticator = authenticator ?? DenyAllSessionAuthenticator.Instance;
    }

    /// <summary>
    /// Await every in-flight spawned <c>send_message</c> turn to completion. <c>send_message</c> runs
    /// its turn as a background task (so the read loop stays free to receive a <c>confirm_tool_action</c>
    /// while a turn is parked). The connection loop calls this in its teardown so an in-flight turn
    /// finishes — and its <c>eventual_response</c> is flushed — before the writer stops (preserves the
    /// graceful-drain contract).
    /// </summary>
    public async Task WaitForTurnsAsync()
    {
        Task[] pending;
        lock (_turnsLock)
        {
            pending = _turnTasks.ToArray();
        }
        if (pending.Length > 0)
        {
            try
            {
                await Task.WhenAll(pending).ConfigureAwait(false);
            }
            catch
            {
                // A turn that faulted already surfaced its own error event; the drain must not throw.
            }
        }
    }

    /// <summary>
    /// Reject every outstanding write-confirmation as denied (fail closed — a write is never
    /// auto-approved on disconnect), so any turn parked on a confirmation unparks and finishes
    /// cleanly. Called by the connection loop on teardown, before <see cref="WaitForTurnsAsync"/>.
    /// </summary>
    public void RejectPendingConfirmations() => _confirmations.RejectAll();

    public async Task DispatchAsync(string rawFrame, Action<JsonObject> sink, CancellationToken cancellationToken = default)
    {
        JsonObject? frame;
        try
        {
            frame = JsonNode.Parse(rawFrame) as JsonObject;
        }
        catch (Exception)
        {
            sink(ProtocolEvents.Error(null, "VALIDATION_ERROR", "Invalid JSON frame"));
            return;
        }

        if (frame is null)
        {
            sink(ProtocolEvents.Error(null, "VALIDATION_ERROR", "Empty or non-object frame"));
            return;
        }

        var action = frame["action"]?.GetValue<string>();
        var requestId = frame["requestId"]?.GetValue<string>();

        try
        {
            switch (action)
            {
                case "ping":
                    sink(ProtocolEvents.Pong(requestId));
                    break;
                case "create_conversation_session":
                    await HandleCreateSessionAsync(frame, requestId, sink, cancellationToken).ConfigureAwait(false);
                    break;
                case "get_session":
                    await HandleGetSessionAsync(frame, requestId, sink, cancellationToken).ConfigureAwait(false);
                    break;
                case "send_message":
                    await HandleSendMessageAsync(frame, requestId, sink, cancellationToken).ConfigureAwait(false);
                    break;
                case "confirm_tool_action":
                    HandleConfirmToolAction(frame, requestId, sink);
                    break;
                case null:
                    sink(ProtocolEvents.Error(requestId, "VALIDATION_ERROR", "Missing 'action'"));
                    break;
                default:
                    sink(ProtocolEvents.Error(requestId, "UNSUPPORTED_ACTION", $"Unsupported action '{action}'"));
                    break;
            }
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            // A handler failed mid-turn (retrieval/embedding/model/DB error, or a bug). Emit a clean
            // error and KEEP the connection alive — never drop the socket with no signal to the
            // client. (Generic message: exception detail stays server-side, not leaked over the wire.)
            sink(ProtocolEvents.Error(requestId, "INTERNAL_ERROR", "Internal error processing the request."));
        }
    }

    private async Task HandleCreateSessionAsync(JsonObject frame, string? requestId, Action<JsonObject> sink, CancellationToken cancellationToken)
    {
        var session = await _store.CreateSessionAsync(
            frame["agentId"]?.GetValue<string>() ?? string.Empty,
            frame["userName"]?.GetValue<string>(),
            frame["userEmail"]?.GetValue<string>(),
            cancellationToken).ConfigureAwait(false);

        var data = new JsonObject
        {
            ["sessionId"] = session.SessionId,
            ["conversationId"] = session.ConversationId,
            ["agentId"] = session.AgentId,
            ["agentName"] = session.AgentName,
            ["userParticipantId"] = session.UserParticipantId,
            ["agentParticipantId"] = session.AgentParticipantId,
        };
        sink(ProtocolEvents.ImmediateResponse(requestId, 200, "Session created", data));
    }

    private async Task HandleGetSessionAsync(JsonObject frame, string? requestId, Action<JsonObject> sink, CancellationToken cancellationToken)
    {
        var session = await _store.GetSessionAsync(frame["sessionId"]?.GetValue<string>() ?? string.Empty, cancellationToken).ConfigureAwait(false);
        if (session is null)
        {
            sink(ProtocolEvents.Error(requestId, "SESSION_NOT_FOUND", "Session not found"));
            return;
        }

        var data = new JsonObject
        {
            ["sessionId"] = session.SessionId,
            ["conversationId"] = session.ConversationId,
            ["agentId"] = session.AgentId,
            ["agentName"] = session.AgentName,
        };
        sink(ProtocolEvents.ImmediateResponse(requestId, 200, "OK", data));
    }

    private async Task HandleSendMessageAsync(JsonObject frame, string? requestId, Action<JsonObject> sink, CancellationToken cancellationToken)
    {
        requestId ??= Guid.NewGuid().ToString();
        var session = await _store.GetSessionAsync(frame["sessionId"]?.GetValue<string>() ?? string.Empty, cancellationToken).ConfigureAwait(false);
        if (session is null)
        {
            sink(ProtocolEvents.Error(requestId, "SESSION_NOT_FOUND", "Session not found"));
            return;
        }

        var message = frame["message"]?.GetValue<string>() ?? string.Empty;

        // 1. Immediate ack (202).
        sink(ProtocolEvents.ImmediateResponse(requestId, 202, "Processing your request...", new JsonObject()));

        // 2. Resolve this agent's per-agent config (instructions.prompt + conversation_workflow) by
        //    the session's agentId. A missing/unknown/malformed config resolves to null → the turn
        //    uses the org/default persona, unchanged. The lookup never throws (the store contract).
        AgentConfig? agentConfig = null;
        if (_agentConfigResolver is not null)
        {
            agentConfig = await _agentConfigResolver.ResolveAsync(session.AgentId, cancellationToken).ConfigureAwait(false);
        }

        // 3. Filter the server's tool set to the agent's tool_config. An agent can only RESTRICT the
        //    DI-provided tools (matched by snake_case toolId), never invent them. A null EnabledTools
        //    (config absent or enabledTools empty) ⇒ the full set, unchanged; a non-null list restricts
        //    to its enabled=true entries (an all-disabled list ⇒ no tools). Unknown toolIds fall out of
        //    the intersection. Mirrors the monorepo AgentToolConfig semantics.
        var enabledTools = agentConfig?.EnabledTools;
        var effectiveTools = enabledTools is null
            ? _tools
            : _tools.Where(t => enabledTools.Any(e => e.Enabled && e.ToolId == t.Name)).ToList();

        // 4. Enforce per-tool authLevel + deliver per-tool config at execution: wrap the surviving tools
        //    so an auth-gated one blocks (with the reference tool message) when its authLevel isn't
        //    satisfied, and a config-bearing one hands its config to the tool. No-op when nothing needs it.
        var gatedTools = ToolAuthGate.Apply(effectiveTools, agentConfig, _authenticator, session.ConversationId);

        // 5. Stream the turn, retrieving through knowledge SCOPED to this connection's access — so a
        //    user only ever sees documents their groups grant (ACL enforced on the chat path).
        var scopedKnowledge = _knowledge?.ForAccess(_access);
        var runner = new TurnRunner(_chatClient, _store, scopedKnowledge, _systemPrompt, _reranker, gatedTools, _confirmTools, _confirmations, agentConfig, _judge);

        // Run the turn as a background task, NOT awaited inline. A turn that calls a
        // confirmation-gated tool PARKS awaiting a later confirm_tool_action frame; the connection's
        // read loop dispatches that frame, so awaiting the turn here would block the reader and
        // deadlock (the confirm could never be read). Spawning frees the reader to receive the
        // confirmation while the turn streams its events through the sink. Mirrors the Rust
        // tokio::spawn / the Python background task. The 202 ack above is already enqueued, and the
        // terminal eventual_response is emitted from the task on completion.
        var requestIdStr = requestId;
        var sessionIdStr = session.SessionId;
        var conversationId = session.ConversationId;

        var task = Task.Run(async () =>
        {
            try
            {
                var result = await runner.RunAsync(conversationId, requestIdStr, message, sink, sessionIdStr, cancellationToken).ConfigureAwait(false);
                sink(ProtocolEvents.EventualResponse(
                    requestIdStr,
                    200,
                    result.MessageId,
                    ProtocolEvents.GeneralResponse(result.Reply),
                    needsEscalation: false,
                    result.Citations));
            }
            catch (OperationCanceledException)
            {
                // Connection torn down mid-turn — nothing to surface; the socket is gone.
            }
            catch (Exception)
            {
                // Mirror the dispatcher's outer guard: a turn failure surfaces a clean error and
                // keeps the connection alive (detail stays server-side).
                sink(ProtocolEvents.Error(requestIdStr, "INTERNAL_ERROR", "Internal error processing the request."));
            }
        }, CancellationToken.None);

        lock (_turnsLock)
        {
            _turnTasks.Add(task);
        }
        _ = task.ContinueWith(t =>
        {
            lock (_turnsLock)
            {
                _turnTasks.Remove(t);
            }
        }, CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
    }

    /// <summary>
    /// <c>confirm_tool_action</c> — resume a turn parked on a write-tool confirmation. Per
    /// <c>spec/actions/confirm-tool-action.schema.json</c> the client replies with
    /// <c>{action, sessionId, requestId, approved}</c> to a <c>write_confirmation_required</c> event.
    /// We resolve the session's pending confirmation with the verdict: the parked <c>IHumanGate</c>
    /// returns and the turn resumes (runs the tool on approve, skips it with a rejection result on
    /// deny). There is no dedicated response event — continuation is signalled by the resumed
    /// streaming sequence; we ack with an <c>immediate_response</c>. Resolving takes the task out, so
    /// a duplicate confirm is a clean <c>NO_PENDING_CONFIRMATION</c> no-op. Fails closed: a missing
    /// <c>sessionId</c> or non-bool <c>approved</c> is rejected (never silently approve).
    /// </summary>
    private void HandleConfirmToolAction(JsonObject frame, string? requestId, Action<JsonObject> sink)
    {
        var sessionId = frame["sessionId"]?.GetValue<string>();
        if (string.IsNullOrEmpty(sessionId))
        {
            sink(ProtocolEvents.Error(requestId, "VALIDATION_ERROR", "confirm_tool_action requires a 'sessionId'"));
            return;
        }

        // `approved` is required and must be a boolean — a missing/garbled verdict must NOT silently
        // approve a write. Fail closed on a bad shape.
        if (frame["approved"] is not JsonValue approvedNode || !approvedNode.TryGetValue<bool>(out var approved))
        {
            sink(ProtocolEvents.Error(requestId, "VALIDATION_ERROR", "confirm_tool_action requires a boolean 'approved'"));
            return;
        }

        if (!_confirmations.Resolve(sessionId, approved))
        {
            sink(ProtocolEvents.Error(requestId, "NO_PENDING_CONFIRMATION", $"no tool action is awaiting confirmation for session '{sessionId}'"));
            return;
        }

        sink(ProtocolEvents.ImmediateResponse(
            requestId,
            200,
            approved ? "Tool action approved" : "Tool action rejected",
            new JsonObject { ["sessionId"] = sessionId, ["approved"] = approved }));
    }
}
