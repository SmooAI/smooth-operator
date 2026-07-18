using System.Text.Json.Nodes;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.Logging;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Core.Extensions;

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
    private readonly IOtpService? _otpService;
    private readonly TurnLimits _limits;
    private readonly ILogger? _logger;

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
        ISessionAuthenticator? authenticator = null,
        IOtpService? otpService = null,
        TurnLimits? limits = null,
        ILogger? logger = null)
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
        // Identity-verification seam for end_user-level tools on public agents. A host's own
        // authenticator wins; absent one, default to the store-backed authenticator so a session
        // marked verified by a successful verify_otp lets its gated tools run (and every un-verified
        // session still fails closed — reads false — exactly like the prior deny-all default).
        _authenticator = authenticator ?? new StoreSessionAuthenticator(_store);
        // Host OTP seam. Absent ⇒ no OTP is ever offered and verify_otp fails closed (unchanged).
        _otpService = otpService;
        // Per-turn token/iteration limits + the resolved model's output ceiling (EPIC th-1cc9fa).
        // Absent ⇒ the raised server defaults (max_tokens 8192, iterations 20) with no ceiling.
        _limits = limits ?? TurnLimits.Default;
        // Optional logger, threaded into each per-turn TurnRunner so a degraded knowledge-retrieval
        // failure surfaces a warning (null ⇒ silent, unchanged for callers that don't wire one).
        _logger = logger;
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
                case "list_conversations":
                    await HandleListConversationsAsync(frame, requestId, sink, cancellationToken).ConfigureAwait(false);
                    break;
                case "send_message":
                    await HandleSendMessageAsync(frame, requestId, sink, cancellationToken).ConfigureAwait(false);
                    break;
                case "confirm_tool_action":
                    HandleConfirmToolAction(frame, requestId, sink);
                    break;
                case "verify_otp":
                    await HandleVerifyOtpAsync(frame, requestId, sink, cancellationToken).ConfigureAwait(false);
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
        // Resume: a `conversationId` naming an existing conversation binds the new session to it
        // (reuse id + persisted history); absent/unknown → a fresh conversation (unchanged). The
        // response echoes session.ConversationId either way, so a resuming client sees the same id
        // it passed. Mirrors the Rust reference's resume branch. th-d5b446.
        var conversationId = frame["conversationId"]?.GetValue<string>();
        var session = await _store.ResumeSessionAsync(
            frame["agentId"]?.GetValue<string>() ?? string.Empty,
            frame["userName"]?.GetValue<string>(),
            frame["userEmail"]?.GetValue<string>(),
            string.IsNullOrEmpty(conversationId) ? null : conversationId,
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

    // Fallback title for a conversation that has messages but no inbound (user) message to preview.
    // The C# store carries no per-conversation name (unlike the Rust reference's conversation.name),
    // so a generic label stands in. th-d5b446.
    private const string DefaultConversationName = "Conversation";
    private const int TitleMaxChars = 60;
    private const int DefaultListLimit = 50;

    /// <summary>
    /// <c>list_conversations</c> — the conversation-sidebar / resume substrate. Returns the store's
    /// conversations that have at least one message (empty ones, minted every page-load, are filtered
    /// out), most-recent-first, each with a short first-inbound title preview + message count. Reply is
    /// an <c>immediate_response</c> carrying <c>{ conversations: [ { conversationId, title, updatedAt,
    /// messageCount } ] }</c>. Optional input: <c>limit</c> (default 50). Mirrors the Rust
    /// <c>handle_list_conversations</c>. th-d5b446.
    /// </summary>
    private async Task HandleListConversationsAsync(JsonObject frame, string? requestId, Action<JsonObject> sink, CancellationToken cancellationToken)
    {
        var limit = DefaultListLimit;
        if (frame["limit"] is JsonValue lv && lv.TryGetValue<int>(out var l) && l > 0)
        {
            limit = l;
        }

        var summaries = await _store.ListConversationsAsync(cancellationToken).ConfigureAwait(false);

        var conversations = new JsonArray();
        foreach (var c in summaries.Where(s => s.MessageCount > 0).OrderByDescending(s => s.UpdatedAt).Take(limit))
        {
            conversations.Add(new JsonObject
            {
                ["conversationId"] = c.ConversationId,
                ["title"] = ConversationTitle(c.FirstInboundText, DefaultConversationName),
                ["updatedAt"] = c.UpdatedAt.ToUniversalTime().UtcDateTime.ToString("o", System.Globalization.CultureInfo.InvariantCulture),
                ["messageCount"] = c.MessageCount,
            });
        }

        sink(ProtocolEvents.ImmediateResponse(requestId, 200, "Conversations", new JsonObject { ["conversations"] = conversations }));
    }

    /// <summary>Sidebar title: a trimmed, ~60-char preview of the first inbound message with leading
    /// markdown/control chars stripped, falling back to <paramref name="fallback"/> when there is no
    /// inbound text. Mirrors the Rust <c>conversation_title</c> (plus the contract's leading-markdown
    /// strip) and the Go/TS parity helpers. th-d5b446.</summary>
    public static string ConversationTitle(string? firstInboundText, string fallback)
    {
        var cleaned = StripLeadingMarkup(firstInboundText ?? string.Empty);
        return cleaned.Length == 0 ? fallback : TruncatePreview(cleaned, TitleMaxChars);
    }

    /// <summary>Trim leading whitespace, control chars, and markdown syntax markers (heading <c>#</c>,
    /// bullets <c>*-</c>, quote <c>&gt;</c>, emphasis <c>_~</c>, code <c>`</c>, cursor <c>▎</c>) off a
    /// preview, so "### Hi" / "- do X" title as "Hi" / "do X".</summary>
    private static string StripLeadingMarkup(string s)
    {
        var i = 0;
        while (i < s.Length && (char.IsWhiteSpace(s[i]) || char.IsControl(s[i]) || "#*>-_~`▎".IndexOf(s[i]) >= 0))
        {
            i++;
        }
        return s[i..];
    }

    /// <summary>Trim and clip to <paramref name="max"/> Unicode scalar values (char-safe, so surrogate
    /// pairs aren't split), appending an ellipsis when clipped. Mirrors the Rust <c>truncate_preview</c>.</summary>
    private static string TruncatePreview(string s, int max)
    {
        s = s.Trim();
        var runes = s.EnumerateRunes().ToArray();
        if (runes.Length <= max)
        {
            return s;
        }
        var sb = new System.Text.StringBuilder();
        for (var i = 0; i < max; i++)
        {
            sb.Append(runes[i].ToString());
        }
        return sb.ToString().TrimEnd() + "…";
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
        // SEP — build the per-turn extension host (default deny via SMOOTH_EXTENSIONS_ALLOW; null when
        // unconfigured, zero overhead). Its tools join the DI tool set so they flow through the SAME
        // per-agent enabled_tools filtering + auth gate below (dotted <ext>.<tool> names match toolId),
        // and its ui/confirm bridges onto write_confirmation_required via the shared confirmation registry.
        var extHost = await ExtensionServerHost.BuildAsync(sink, requestId, session.SessionId, _confirmations).ConfigureAwait(false);
        var baseTools = extHost is null ? _tools : _tools.Concat(extHost.Tools()).ToList();

        // Scope retrieval to THIS connection's access up front — the same handle grounds the turn's
        // auto-context RAG (step 5 below) AND backs the built-in knowledge_search tool, so both read
        // through the same ACL-filtered store (a doc the caller's groups don't grant is never a candidate).
        var scopedKnowledge = _knowledge?.ForAccess(_access);

        // Built-in knowledge_search: a model-callable search over the connection's ACL-scoped knowledge
        // (parity with the Rust server's KnowledgeSearchTool). Prepended before the enabled_tools filter
        // so it flows through the SAME per-agent restriction + auth gate as every other tool — an agent
        // with no tool_config gets it (like a Rust built-in), one that restricts tools must list
        // "knowledge_search" to keep it. Null (skipped) when no knowledge store is configured.
        var knowledgeTool = KnowledgeSearchTool.Create(scopedKnowledge);
        if (knowledgeTool is not null)
        {
            baseTools = baseTools.Prepend(knowledgeTool).ToList();
        }

        var enabledTools = agentConfig?.EnabledTools;
        var effectiveTools = enabledTools is null
            ? baseTools
            : baseTools.Where(t => enabledTools.Any(e => e.Enabled && e.ToolId == t.Name)).ToList();

        // 4. Enforce per-tool authLevel + deliver per-tool config at execution: wrap the surviving tools
        //    so an auth-gated one blocks (with the reference tool message) when its authLevel isn't
        //    satisfied, and a config-bearing one hands its config to the tool. No-op when nothing needs it.
        //    When a host OTP service is installed, a per-turn recorder captures an end_user tool the gate
        //    refused for lack of verification, so the OTP flow can be offered after the turn.
        var otpRecorder = _otpService is not null ? new OtpRefusalRecorder() : null;
        var gatedTools = ToolAuthGate.Apply(effectiveTools, agentConfig, _authenticator, session.ConversationId, otpRecorder);

        // 4b. Resolve the write-confirmation (HITL) patterns for THIS agent. A per-agent
        //     ConfirmToolPatterns (from AgentConfig) overrides the global ConfirmTools singleton, so a
        //     multi-agent host gates writes per agent; a null (agent didn't specify) falls back to the
        //     global patterns — backward compatible. An empty per-agent list is an explicit "no gating
        //     for this agent" that still overrides the global.
        var confirmTools = agentConfig?.ConfirmToolPatterns ?? _confirmTools;

        // 5. Stream the turn, retrieving through knowledge SCOPED to this connection's access (computed
        //    above, and reused to back the built-in knowledge_search tool) — so a user only ever sees
        //    documents their groups grant (ACL enforced on the chat path).
        var runner = new TurnRunner(_chatClient, _store, scopedKnowledge, _systemPrompt, _reranker, gatedTools, confirmTools, _confirmations, agentConfig, _judge, _limits, _logger);

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
        var userEmail = session.UserEmail;

        var task = Task.Run(async () =>
        {
            try
            {
                var result = await runner.RunAsync(conversationId, requestIdStr, message, sink, sessionIdStr, cancellationToken).ConfigureAwait(false);

                // If the auth gate refused an end_user tool this turn for lack of a verified session,
                // and a host OTP service is installed and the session has a contact to reach, offer the
                // OTP flow (prompt → dispatch → ack) BEFORE the terminal response. Like the Rust
                // reference, the server does not park/auto-resume: the client verifies via verify_otp
                // and re-sends its message once the session is authenticated.
                if (_otpService is not null && otpRecorder?.Refused is string refusedTool)
                {
                    var contact = new OtpContact(Email: userEmail);
                    if (!contact.IsEmpty)
                    {
                        await OfferOtpAsync(sessionIdStr, refusedTool, contact, requestIdStr, sink, cancellationToken).ConfigureAwait(false);
                    }
                }

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
            finally
            {
                // SEP — tear down the per-turn extension host (kills its subprocesses). The
                // confirmation registry it may have parked on is cleared by the TurnRunner's finally.
                if (extHost is not null)
                {
                    await extHost.ShutdownAllAsync().ConfigureAwait(false);
                }
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

    /// <summary>
    /// Emit the OTP-offer sequence for a turn whose <c>end_user</c> tool was refused for lack of a
    /// verified session: <c>otp_verification_required</c> (prompt the client), then
    /// <see cref="IOtpService.SendOtpAsync"/> on the host service, then <c>otp_sent</c> (ack delivery)
    /// — or an <c>OTP_SEND_FAILED</c> error if delivery throws. The masked destination + channel come
    /// from the host; the server never sees the code. <c>authLevel</c> is fixed <c>end_user</c> (the
    /// only level this flow remedies). The C# analog of the Rust <c>offer_otp</c>.
    /// </summary>
    private async Task OfferOtpAsync(string sessionId, string tool, OtpContact contact, string requestId, Action<JsonObject> sink, CancellationToken cancellationToken)
    {
        sink(ProtocolEvents.OtpVerificationRequired(
            requestId,
            tool,
            $"Verify your identity to continue using '{tool}'.",
            contact.AvailableChannels,
            "end_user"));
        try
        {
            var delivery = await _otpService!.SendOtpAsync(sessionId, contact, cancellationToken).ConfigureAwait(false);
            sink(ProtocolEvents.OtpSent(requestId, delivery.Channel.ToWire(), delivery.MaskedDestination));
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            sink(ProtocolEvents.Error(requestId, "OTP_SEND_FAILED", "Failed to send verification code."));
        }
    }

    /// <summary>
    /// <c>verify_otp</c> — validate a submitted OTP code and, on success, mark the session
    /// identity-verified. Per <c>spec/actions/verify-otp.schema.json</c> the client replies with
    /// <c>{action, sessionId, requestId, code}</c> to an <c>otp_verification_required</c> event. There
    /// is no dedicated response event: a correct code emits <c>otp_verified</c> (the client then
    /// re-sends its message to run the gated tool — the reference server does not park/auto-resume the
    /// original turn); a rejected code emits <c>otp_invalid</c> with the host's remaining-attempt count.
    /// With no <see cref="IOtpService"/> installed, verification is impossible, so we fail closed with
    /// an <c>otp_invalid</c> (<c>NOT_FOUND</c>, 0 attempts). Validation order mirrors the Rust
    /// reference: requestId, sessionId, code, session-exists, then service. The C# analog of
    /// <c>handle_verify_otp</c>.
    /// </summary>
    private async Task HandleVerifyOtpAsync(JsonObject frame, string? requestId, Action<JsonObject> sink, CancellationToken cancellationToken)
    {
        // requestId is load-bearing (it echoes the originating otp_verification_required); require it.
        if (string.IsNullOrEmpty(requestId))
        {
            sink(ProtocolEvents.Error(null, "VALIDATION_ERROR", "verify_otp requires a 'requestId'"));
            return;
        }

        var sessionId = frame["sessionId"]?.GetValue<string>();
        if (string.IsNullOrEmpty(sessionId))
        {
            sink(ProtocolEvents.Error(requestId, "VALIDATION_ERROR", "verify_otp requires a 'sessionId'"));
            return;
        }

        var code = frame["code"]?.GetValue<string>();
        if (string.IsNullOrEmpty(code))
        {
            sink(ProtocolEvents.Error(requestId, "VALIDATION_ERROR", "verify_otp requires a 'code'"));
            return;
        }

        // The session must exist (a code can't verify a session we don't track).
        var session = await _store.GetSessionAsync(sessionId, cancellationToken).ConfigureAwait(false);
        if (session is null)
        {
            sink(ProtocolEvents.Error(requestId, "SESSION_NOT_FOUND", $"session '{sessionId}' not found"));
            return;
        }

        // No host OTP service → verification is impossible. Fail closed on the documented otp_invalid
        // path (a client shouldn't reach here without first receiving otp_verification_required, which
        // only an installed service emits).
        if (_otpService is null)
        {
            sink(ProtocolEvents.OtpInvalid(requestId, "NOT_FOUND", 0, "No verification is in progress for this session."));
            return;
        }

        var outcome = await _otpService.VerifyOtpAsync(sessionId, code, cancellationToken).ConfigureAwait(false);
        switch (outcome)
        {
            case OtpVerifyOutcome.Verified:
                await _store.SetSessionAuthenticatedAsync(session.ConversationId, true, cancellationToken).ConfigureAwait(false);
                sink(ProtocolEvents.OtpVerified(requestId, "Identity verified successfully."));
                break;
            case OtpVerifyOutcome.Invalid invalid:
                sink(ProtocolEvents.OtpInvalid(requestId, invalid.Error?.ToWire(), invalid.AttemptsRemaining, invalid.Message));
                break;
        }
    }
}
