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

    public FrameDispatcher(
        ISessionStore store,
        IChatClient chatClient,
        IAccessKnowledge? knowledge = null,
        AccessContext? access = null,
        string? systemPrompt = null,
        IReranker? reranker = null)
    {
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _chatClient = chatClient ?? throw new ArgumentNullException(nameof(chatClient));
        _knowledge = knowledge;
        _access = access ?? AccessContext.Anonymous;
        _systemPrompt = systemPrompt;
        _reranker = reranker;
    }

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

        // 2. Stream the turn, retrieving through knowledge SCOPED to this connection's access — so a
        //    user only ever sees documents their groups grant (ACL enforced on the chat path).
        var scopedKnowledge = _knowledge?.ForAccess(_access);
        var runner = new TurnRunner(_chatClient, _store, scopedKnowledge, _systemPrompt, _reranker);
        var result = await runner.RunAsync(session.ConversationId, requestId, message, sink, cancellationToken).ConfigureAwait(false);

        // 3. Terminal eventual_response.
        sink(ProtocolEvents.EventualResponse(
            requestId,
            200,
            result.MessageId,
            ProtocolEvents.GeneralResponse(result.Reply),
            needsEscalation: false,
            result.Citations));
    }
}
