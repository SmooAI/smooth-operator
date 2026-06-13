using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Routes an incoming protocol frame (by its <c>action</c> discriminator) to the right handler and
/// emits the response event(s) to <paramref>sink</paramref>. The C# analog of the Rust server's
/// <c>handle_frame</c>. Transport-agnostic: a WebSocket host (later phase) calls
/// <see cref="DispatchAsync"/> per inbound frame and writes the sink's events back over the socket.
/// </summary>
public sealed class FrameDispatcher
{
    private readonly ISessionStore _store;
    private readonly TurnRunner _runner;

    public FrameDispatcher(ISessionStore store, TurnRunner runner)
    {
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _runner = runner ?? throw new ArgumentNullException(nameof(runner));
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

        switch (action)
        {
            case "ping":
                sink(ProtocolEvents.Pong(requestId));
                break;
            case "create_conversation_session":
                HandleCreateSession(frame, requestId, sink);
                break;
            case "get_session":
                HandleGetSession(frame, requestId, sink);
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

    private void HandleCreateSession(JsonObject frame, string? requestId, Action<JsonObject> sink)
    {
        var session = _store.CreateSession(
            frame["agentId"]?.GetValue<string>() ?? string.Empty,
            frame["userName"]?.GetValue<string>(),
            frame["userEmail"]?.GetValue<string>());

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

    private void HandleGetSession(JsonObject frame, string? requestId, Action<JsonObject> sink)
    {
        var session = _store.GetSession(frame["sessionId"]?.GetValue<string>() ?? string.Empty);
        if (session is null)
        {
            sink(ProtocolEvents.Error(requestId, "NOT_FOUND", "Session not found"));
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
        var session = _store.GetSession(frame["sessionId"]?.GetValue<string>() ?? string.Empty);
        if (session is null)
        {
            sink(ProtocolEvents.Error(requestId, "NOT_FOUND", "Session not found"));
            return;
        }

        var message = frame["message"]?.GetValue<string>() ?? string.Empty;

        // 1. Immediate ack (202).
        sink(ProtocolEvents.ImmediateResponse(requestId, 202, "Processing your request...", new JsonObject()));

        // 2. Stream the turn (emits stream_token events; returns reply + citations).
        var result = await _runner.RunAsync(session.ConversationId, requestId, message, sink, cancellationToken).ConfigureAwait(false);

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
