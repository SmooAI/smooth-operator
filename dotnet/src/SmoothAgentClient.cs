// SmoothAgentClient — an idiomatic, transport-agnostic async client for the
// smooth-operator WebSocket protocol.
//
// Design goals
// ------------
//  • Transport-agnostic. The client never touches a real socket directly; it talks
//    to an injectable ITransport. The default (WebSocketTransport) uses
//    ClientWebSocket, but tests inject a mock.
//  • Request/response correlation by `requestId`. Every action gets a generated
//    requestId; the client routes incoming events back to the originating call.
//  • Streaming as IAsyncEnumerable. SendMessageAsync returns a MessageTurn that is
//    both async-iterable (yields each stream_token / stream_chunk / HITL event in
//    order via WithCancellation / await foreach) and awaitable for the terminal
//    eventual_response (await turn.Completion). This models the
//    stream_token/stream_chunk → eventual_response flow without a callback style.
//  • HITL resumes (confirm_tool_action / verify_otp) route back into the originating
//    turn because they reuse its requestId.
//  • No live server required — fully unit-testable with a mock transport.

using System.Collections.Concurrent;
using System.Text.Json;
using System.Threading.Channels;

namespace SmooAI.SmoothOperator;

/// <summary>A protocol-level error surfaced as an exception.</summary>
public sealed class ProtocolException : Exception
{
    public string Code { get; }
    public string? RequestId { get; }

    public ProtocolException(string code, string message, string? requestId = null) : base(message)
    {
        Code = code;
        RequestId = requestId;
    }
}

/// <summary>Raised when a single-response request times out before a terminal event arrives.</summary>
public sealed class RequestTimeoutException : Exception
{
    public string RequestId { get; }

    public RequestTimeoutException(string requestId, TimeSpan timeout)
        : base($"Request {requestId} timed out after {timeout.TotalMilliseconds}ms")
        => RequestId = requestId;
}

public sealed class SmoothAgentClientOptions
{
    /// <summary>WebSocket URL, e.g. <c>wss://realtime.prod.smooth-agent.dev</c>.</summary>
    public string Url { get; set; } = string.Empty;

    /// <summary>
    /// Optional connection auth token. When set, the default transport's connect URL
    /// carries it in the <c>?token=</c> query slot (browsers can't set WebSocket
    /// handshake headers), which a token-gated (local-flavor) server reads to
    /// authenticate the connection. Merged with any existing query on <see cref="Url"/>.
    /// Ignored when a custom <see cref="Transport"/> is supplied.
    /// </summary>
    public string? Token { get; set; }

    /// <summary>Inject a transport (for tests / custom sockets). Defaults to a WebSocket transport over <see cref="Url"/>.</summary>
    public ITransport? Transport { get; set; }

    /// <summary>Generate request IDs. Defaults to <c>req-{guid}</c>.</summary>
    public Func<string>? GenerateRequestId { get; set; }

    /// <summary>Per-request timeout for non-streaming actions. Default 30s. Use <see cref="Timeout.InfiniteTimeSpan"/> to disable.</summary>
    public TimeSpan RequestTimeout { get; set; } = TimeSpan.FromSeconds(30);

    /// <summary>Serializer options for (de)serializing frames. Defaults to camelCase-friendly defaults.</summary>
    public JsonSerializerOptions? JsonOptions { get; set; }
}

/// <summary>
/// A streaming message turn. Async-iterate it (<c>await foreach (var ev in turn)</c>)
/// to receive every intermediate event in arrival order, and/or await
/// <see cref="Completion"/> for the terminal <see cref="EventualResponseEvent"/>.
/// </summary>
public sealed class MessageTurn : IAsyncEnumerable<ServerEvent>
{
    /// <summary>The requestId this turn is correlated on.</summary>
    public string RequestId { get; }

    private readonly Channel<ServerEvent> _channel =
        Channel.CreateUnbounded<ServerEvent>(new UnboundedChannelOptions { SingleReader = false, SingleWriter = true });
    private readonly TaskCompletionSource<EventualResponseEvent> _completion =
        new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly Action _onClose;
    private int _done;

    internal MessageTurn(string requestId, Action onClose)
    {
        RequestId = requestId;
        _onClose = onClose;
    }

    /// <summary>Resolves with the terminal <c>eventual_response</c>, or faults with a <see cref="ProtocolException"/>.</summary>
    public Task<EventualResponseEvent> Completion => _completion.Task;

    /// <summary>Feed an event into the turn (called by the client's dispatcher).</summary>
    internal void Push(ServerEvent ev)
    {
        if (Volatile.Read(ref _done) != 0) return;

        switch (ev)
        {
            case ErrorEvent err:
                _channel.Writer.TryWrite(err);
                Finish(null, new ProtocolException(
                    err.Data.Error.Code is { Length: > 0 } c ? c : "INTERNAL_ERROR",
                    err.Data.Error.Message is { Length: > 0 } m ? m : "Unknown protocol error",
                    RequestId));
                break;

            case EventualResponseEvent done:
                _channel.Writer.TryWrite(done);
                Finish(done, null);
                break;

            default:
                _channel.Writer.TryWrite(ev);
                break;
        }
    }

    /// <summary>Force-close the turn (e.g. on disconnect) with an error.</summary>
    internal void Abort(Exception error) => Finish(null, error);

    private void Finish(EventualResponseEvent? final, Exception? error)
    {
        if (Interlocked.Exchange(ref _done, 1) != 0) return;

        _channel.Writer.TryComplete();
        _onClose();

        if (error is not null) _completion.TrySetException(error);
        else if (final is not null) _completion.TrySetResult(final);
        else _completion.TrySetException(new ProtocolException("ABORTED", "Turn aborted without a terminal event", RequestId));
    }

    public async IAsyncEnumerator<ServerEvent> GetAsyncEnumerator(CancellationToken cancellationToken = default)
    {
        var reader = _channel.Reader;
        while (await reader.WaitToReadAsync(cancellationToken).ConfigureAwait(false))
        {
            while (reader.TryRead(out var ev))
                yield return ev;
        }
        // If the turn finished with an error, surface it to the iterator too.
        if (_completion.Task.IsFaulted)
            throw _completion.Task.Exception!.GetBaseException();
    }
}

public sealed class SmoothAgentClient : IAsyncDisposable
{
    private readonly ITransport _transport;
    private readonly Func<string> _generateRequestId;
    private readonly TimeSpan _requestTimeout;
    private readonly JsonSerializerOptions _json;

    /// <summary>requestId → single-response waiter (create_session, get_session, ping, …).</summary>
    private readonly ConcurrentDictionary<string, PendingRequest> _pending = new();
    /// <summary>requestId → active streaming turn (send_message, and its HITL resumes).</summary>
    private readonly ConcurrentDictionary<string, MessageTurn> _turns = new();
    /// <summary>Unsolicited-event listeners (keepalive, server push).</summary>
    private event Action<ServerEvent>? _listeners;

    public SmoothAgentClient(SmoothAgentClientOptions options)
    {
        _transport = options.Transport ?? new WebSocketTransport(WithToken(options.Url, options.Token));
        _requestTimeout = options.RequestTimeout;
        _generateRequestId = options.GenerateRequestId ?? (() => $"req-{Guid.NewGuid():N}");
        _json = options.JsonOptions ?? new JsonSerializerOptions(JsonSerializerDefaults.Web)
        {
            DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
        };

        _transport.Message += HandleFrame;
        _transport.Closed += OnTransportClosed;
    }

    /// <summary>Subscribe to unsolicited / uncorrelated server events (e.g. keepalive).</summary>
    public event Action<ServerEvent>? Event
    {
        add => _listeners += value;
        remove => _listeners -= value;
    }

    /// <summary>Open the underlying transport.</summary>
    public Task ConnectAsync(CancellationToken cancellationToken = default)
        => _transport.ConnectAsync(cancellationToken);

    /// <summary>Close the transport and fault all in-flight work.</summary>
    public async Task DisconnectAsync(string reason = "client disconnect")
    {
        FailAll(new InvalidOperationException(reason));
        _transport.Message -= HandleFrame;
        _transport.Closed -= OnTransportClosed;
        await _transport.CloseAsync(1000, reason).ConfigureAwait(false);
    }

    /// <summary>
    /// Merge a connection <paramref name="token"/> into <paramref name="url"/>'s
    /// <c>?token=</c> query slot, preserving any existing query parameters. Returns
    /// <paramref name="url"/> unchanged when the token is null/empty. The token value is
    /// percent-encoded; a pre-existing <c>token</c> param is replaced.
    /// </summary>
    public static string WithToken(string url, string? token)
    {
        if (string.IsNullOrEmpty(token)) return url;

        var builder = new UriBuilder(url);
        var query = System.Web.HttpUtility.ParseQueryString(builder.Query);
        query.Set("token", token);
        builder.Query = query.ToString();
        return builder.Uri.ToString();
    }

    // ───────────────────────────── Actions ─────────────────────────────────

    /// <summary>Start a new conversation session. Resolves with the session descriptor.</summary>
    public async Task<CreateConversationSessionResult> CreateConversationSessionAsync(
        CreateConversationSessionAction request, CancellationToken cancellationToken = default)
    {
        var ev = await RequestAsync(request, cancellationToken).ConfigureAwait(false);
        return ExtractImmediateData<CreateConversationSessionResult>(ev);
    }

    /// <summary>Fetch a session snapshot by ID.</summary>
    public async Task<SessionResult> GetSessionAsync(
        GetSessionAction request, CancellationToken cancellationToken = default)
    {
        var ev = await RequestAsync(request, cancellationToken).ConfigureAwait(false);
        return ExtractImmediateData<SessionResult>(ev);
    }

    /// <summary>Fetch a page of conversation messages.</summary>
    public async Task<GetMessagesResult> GetMessagesAsync(
        GetMessagesAction request, CancellationToken cancellationToken = default)
    {
        var ev = await RequestAsync(request, cancellationToken).ConfigureAwait(false);
        return ExtractImmediateData<GetMessagesResult>(ev);
    }

    /// <summary>Keepalive ping. Resolves with the server timestamp from the <c>pong</c> event.</summary>
    public async Task<long> PingAsync(CancellationToken cancellationToken = default)
    {
        var ev = await RequestAsync(new PingAction(), cancellationToken).ConfigureAwait(false);
        if (ev is PongEvent pong)
            return pong.Timestamp ?? pong.Data?.Timestamp ?? DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        return DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
    }

    /// <summary>
    /// Submit a user message and return a <see cref="MessageTurn"/>: async-iterate it for
    /// the streaming events, and/or await <see cref="MessageTurn.Completion"/> for the
    /// terminal <c>eventual_response</c>.
    /// </summary>
    public MessageTurn SendMessageAsync(SendMessageAction request)
    {
        var requestId = request.RequestId ?? _generateRequestId();
        request.RequestId = requestId;

        var turn = new MessageTurn(requestId, () => _turns.TryRemove(requestId, out _));
        _turns[requestId] = turn;

        try
        {
            _ = _transport.SendAsync(Serialize(request));
        }
        catch (Exception ex)
        {
            _turns.TryRemove(requestId, out _);
            turn.Abort(ex);
        }
        return turn;
    }

    /// <summary>
    /// Approve or reject a pending tool write, resuming the paused turn identified by
    /// <paramref name="requestId"/>. The resumed streaming events flow back into the
    /// original <see cref="MessageTurn"/> for that requestId.
    /// </summary>
    public Task ConfirmToolActionAsync(string sessionId, string requestId, bool approved,
        CancellationToken cancellationToken = default)
        => _transport.SendAsync(Serialize(new ConfirmToolAction
        {
            SessionId = sessionId,
            RequestId = requestId,
            Approved = approved,
        }), cancellationToken);

    /// <summary>
    /// Submit an OTP code, resuming the paused turn identified by <paramref name="requestId"/>.
    /// The resumed streaming events flow back into the original <see cref="MessageTurn"/>.
    /// </summary>
    public Task VerifyOtpAsync(string sessionId, string requestId, string code,
        CancellationToken cancellationToken = default)
        => _transport.SendAsync(Serialize(new VerifyOtpAction
        {
            SessionId = sessionId,
            RequestId = requestId,
            Code = code,
        }), cancellationToken);

    // ─────────────────────────── Internals ─────────────────────────────────

    /// <summary>Send an action that expects a single correlated response event.</summary>
    private Task<ServerEvent> RequestAsync(ClientAction action, CancellationToken cancellationToken)
    {
        var requestId = action.RequestId ?? _generateRequestId();
        action.RequestId = requestId;

        var tcs = new TaskCompletionSource<ServerEvent>(TaskCreationOptions.RunContinuationsAsynchronously);
        CancellationTokenSource? timeoutCts = null;
        CancellationTokenRegistration ctReg = default;

        var pending = new PendingRequest(tcs);
        _pending[requestId] = pending;

        if (_requestTimeout != Timeout.InfiniteTimeSpan && _requestTimeout > TimeSpan.Zero)
        {
            timeoutCts = new CancellationTokenSource(_requestTimeout);
            timeoutCts.Token.Register(() =>
            {
                if (_pending.TryRemove(requestId, out _))
                    tcs.TrySetException(new RequestTimeoutException(requestId, _requestTimeout));
            });
        }

        if (cancellationToken.CanBeCanceled)
        {
            ctReg = cancellationToken.Register(() =>
            {
                if (_pending.TryRemove(requestId, out _))
                    tcs.TrySetCanceled(cancellationToken);
            });
        }

        // Cleanup the timeout/registration once the request settles.
        tcs.Task.ContinueWith(_ =>
        {
            timeoutCts?.Dispose();
            ctReg.Dispose();
        }, TaskScheduler.Default);

        try
        {
            _ = _transport.SendAsync(Serialize(action), cancellationToken);
        }
        catch (Exception ex)
        {
            _pending.TryRemove(requestId, out _);
            tcs.TrySetException(ex);
        }

        return tcs.Task;
    }

    /// <summary>Parse and route an incoming frame to the right consumer.</summary>
    private void HandleFrame(string data)
    {
        ServerEvent? ev;
        try
        {
            ev = JsonSerializer.Deserialize<ServerEvent>(data, _json);
        }
        catch (JsonException)
        {
            return; // ignore malformed / unknown frames
        }
        if (ev is null) return;

        var requestId = ev.RequestId;

        // 1. Streaming turn? Route every related event into it.
        if (requestId is not null && _turns.TryGetValue(requestId, out var turn))
        {
            turn.Push(ev);
            return;
        }

        // 2. Single-response request awaiting resolution?
        if (requestId is not null && _pending.TryRemove(requestId, out var pending))
        {
            if (ev is ErrorEvent err)
            {
                pending.Tcs.TrySetException(new ProtocolException(
                    err.Data.Error.Code is { Length: > 0 } c ? c : "INTERNAL_ERROR",
                    err.Data.Error.Message is { Length: > 0 } m ? m : "Unknown protocol error",
                    requestId));
            }
            else
            {
                pending.Tcs.TrySetResult(ev);
            }
            return;
        }

        // 3. Unsolicited / uncorrelated event (keepalive, server push).
        _listeners?.Invoke(ev);
    }

    private void OnTransportClosed(TransportCloseInfo info)
        => FailAll(new InvalidOperationException(
            $"Transport closed{(info.Reason is { Length: > 0 } r ? $": {r}" : string.Empty)}"));

    private void FailAll(Exception error)
    {
        foreach (var kv in _pending)
        {
            if (_pending.TryRemove(kv.Key, out var p))
                p.Tcs.TrySetException(error);
        }
        foreach (var kv in _turns)
        {
            if (_turns.TryRemove(kv.Key, out var t))
                t.Abort(error);
        }
    }

    private string Serialize(ClientAction action)
        => JsonSerializer.Serialize(action, action.GetType(), _json);

    /// <summary>Pull the typed <c>data</c> payload out of an <c>immediate_response</c> event.</summary>
    private T ExtractImmediateData<T>(ServerEvent ev)
    {
        if (ev is ImmediateResponseEvent imm && imm.Data is { } el)
        {
            var decoded = el.Deserialize<T>(_json);
            if (decoded is not null) return decoded;
        }
        throw new ProtocolException("UNEXPECTED_EVENT", $"Expected immediate_response with data, got \"{ev.Type}\"", ev.RequestId);
    }

    public async ValueTask DisposeAsync()
    {
        await DisconnectAsync().ConfigureAwait(false);
        if (_transport is IAsyncDisposable d) await d.DisposeAsync().ConfigureAwait(false);
    }

    private readonly record struct PendingRequest(TaskCompletionSource<ServerEvent> Tcs);
}

// ──────────────────── Decoded response payloads ────────────────────
// Strongly-typed results for the non-streaming actions, matching the spec Response
// schemas. (The wire payloads ride inside an immediate_response's `data`.)

public sealed class CreateConversationSessionResult
{
    public string SessionId { get; set; } = string.Empty;
    public string ConversationId { get; set; } = string.Empty;
    public string AgentId { get; set; } = string.Empty;
    public string AgentName { get; set; } = string.Empty;
    public string UserParticipantId { get; set; } = string.Empty;
    public string AgentParticipantId { get; set; } = string.Empty;
}

public sealed class SessionResult
{
    public string SessionId { get; set; } = string.Empty;
    public string ConversationId { get; set; } = string.Empty;
    public string AgentId { get; set; } = string.Empty;
    public string AgentName { get; set; } = string.Empty;
    public string UserParticipantId { get; set; } = string.Empty;
    public string AgentParticipantId { get; set; } = string.Empty;
    public string? ThreadId { get; set; }
    public string? Status { get; set; }
}

public sealed class GetMessagesResult
{
    public List<MessageItem> Messages { get; set; } = new();
    public bool HasMore { get; set; }
}

public sealed class MessageItem
{
    public string Id { get; set; } = string.Empty;
    public string Direction { get; set; } = string.Empty;
    public JsonElement? Content { get; set; }
    public string CreatedAt { get; set; } = string.Empty;
}
